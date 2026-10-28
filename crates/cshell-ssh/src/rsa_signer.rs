//! RSA private operations use the already pinned ring backend, with bounded
//! component admission and one-use, identity-bound SSH authentication signers.
use crypto_bigint::{BoxedUint, NonZero};
use ring::{
    rand::SystemRandom,
    rsa::{KeyPair, KeyPairComponents, PublicKeyComponents},
    signature,
};
use russh::keys::{
    agent::AgentIdentity,
    ssh_key::{HashAlg, Mpint, PrivateKey, PublicKey},
};
use std::sync::{Arc, OnceLock};
use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum RsaKeyError {
    #[error(
        "RSA keys require a 2048–4096 bit modulus and a bounded odd exponent of at least 65537"
    )]
    Parameters,
    #[error("RSA modulus must have 2048–4096 bits")]
    ModulusBits,
    #[error(
        "RSA public exponent must be odd and between 65537 and 8589934591; modulus size is checked separately"
    )]
    PublicExponent,
    #[error("RSA private components or public/private consistency are invalid")]
    Consistency,
    #[error("RSA signing requires the exact reviewed identity and SHA-2 algorithm")]
    Identity,
    #[error("RSA cryptographic worker budget is busy; retry explicitly later")]
    Busy,
    #[error("RSA signing worker could not complete")]
    Worker,
    #[error("RSA authentication transport closed")]
    Transport,
}
impl From<russh::SendError> for RsaKeyError {
    fn from(_: russh::SendError) -> Self {
        Self::Transport
    }
}
fn permit() -> Result<tokio::sync::OwnedSemaphorePermit, RsaKeyError> {
    static BUDGET: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    BUDGET
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| RsaKeyError::Busy)
}
fn positive(value: &Mpint) -> Result<&[u8], RsaKeyError> {
    let bytes = value
        .as_positive_bytes()
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .ok_or(RsaKeyError::Parameters)?;
    Ok(bytes)
}
pub(super) fn validate_public(public: &PublicKey) -> Result<(), RsaKeyError> {
    let rsa = public.key_data().rsa().ok_or(RsaKeyError::Parameters)?;
    let n = positive(rsa.n())?;
    let bits = n.len() * 8 - n[0].leading_zeros() as usize;
    let e = positive(rsa.e())?;
    if !(2048..=4096).contains(&bits) {
        return Err(RsaKeyError::ModulusBits);
    }
    if e.len() > 5 {
        return Err(RsaKeyError::PublicExponent);
    }
    if n[n.len() - 1] & 1 == 0 {
        return Err(RsaKeyError::Consistency);
    }
    let exponent = e.iter().fold(0u64, |v, b| (v << 8) | u64::from(*b));
    if !(65537..=(1u64 << 33) - 1).contains(&exponent) || exponent & 1 == 0 {
        return Err(RsaKeyError::PublicExponent);
    }
    Ok(())
}
pub(super) fn crt_exponent(d: &[u8], prime: &[u8]) -> Result<Zeroizing<Box<[u8]>>, RsaKeyError> {
    // Fixed precision, bounded by admission above; no variable-time division is
    // selected for secret components. Arithmetic is delegated to crypto-bigint.
    let d = Zeroizing::new(BoxedUint::from_be_slice(d, 4096).map_err(|_| RsaKeyError::Parameters)?);
    let p =
        Zeroizing::new(BoxedUint::from_be_slice(prime, 4096).map_err(|_| RsaKeyError::Parameters)?);
    let minus_one = p.wrapping_sub(BoxedUint::one_with_precision(4096));
    let modulus = Zeroizing::new(
        NonZero::new(minus_one)
            .into_option()
            .ok_or(RsaKeyError::Consistency)?,
    );
    let result = Zeroizing::new(d.rem(&*modulus));
    let padded = Zeroizing::new(result.to_be_bytes());
    let first = padded
        .iter()
        .position(|b| *b != 0)
        .ok_or(RsaKeyError::Consistency)?;
    Ok(Zeroizing::new(padded[first..].to_vec().into_boxed_slice()))
}

pub(super) struct RsaSigningKey {
    key: KeyPair,
    public: PublicKey,
}
impl RsaSigningKey {
    pub(super) fn from_key(key: &PrivateKey) -> Result<Arc<Self>, RsaKeyError> {
        let _permit = permit()?;
        validate_public(key.public_key())?;
        let rsa = key.key_data().rsa().ok_or(RsaKeyError::Parameters)?;
        let private = rsa.private();
        let d = positive(private.d())?;
        let p = positive(private.p())?;
        let q = positive(private.q())?;
        let q_inv = positive(private.iqmp())?;
        // Do not wrap 1-1 into a nonzero divisor; other structural restrictions
        // and the supplied CRT inverse are checked by the backend constructor.
        if p == [1] || q == [1] {
            return Err(RsaKeyError::Consistency);
        }
        let dp = crt_exponent(d, p)?;
        let dq = crt_exponent(d, q)?;
        let key = KeyPair::from_components(&KeyPairComponents {
            public_key: PublicKeyComponents {
                n: positive(rsa.public().n())?,
                e: positive(rsa.public().e())?,
            },
            d,
            p,
            q,
            dP: &dp[..],
            dQ: &dq[..],
            qInv: q_inv,
        })
        .map_err(|_| RsaKeyError::Consistency)?;
        let result = Arc::new(Self {
            key,
            public: key_public(rsa.public()),
        });
        let mut challenge = b"cshell-rsa-key-consistency\0".to_vec();
        challenge.extend_from_slice(&rand::random::<[u8; 32]>());
        // Verify both wire algorithms before admitting a key. The source d is
        // reduced into both CRT exponents; no alternate private exponent is used.
        for hash in [HashAlg::Sha256, HashAlg::Sha512] {
            result.sign_checked(hash, &challenge)?;
        }
        Ok(result)
    }
    fn sign_checked(&self, hash: HashAlg, message: &[u8]) -> Result<Vec<u8>, RsaKeyError> {
        let (encoding, verification): (
            &'static dyn signature::RsaEncoding,
            &signature::RsaParameters,
        ) = match hash {
            HashAlg::Sha256 => (
                &signature::RSA_PKCS1_SHA256,
                &signature::RSA_PKCS1_2048_8192_SHA256,
            ),
            HashAlg::Sha512 => (
                &signature::RSA_PKCS1_SHA512,
                &signature::RSA_PKCS1_2048_8192_SHA512,
            ),
            _ => return Err(RsaKeyError::Identity),
        };
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(encoding, &SystemRandom::new(), message, &mut signature)
            .map_err(|_| RsaKeyError::Consistency)?;
        let public = self
            .public
            .key_data()
            .rsa()
            .ok_or(RsaKeyError::Parameters)?;
        PublicKeyComponents {
            n: positive(public.n())?,
            e: positive(public.e())?,
        }
        .verify(verification, message, &signature)
        .map_err(|_| RsaKeyError::Consistency)?;
        Ok(signature)
    }
}
fn key_public(rsa: &russh::keys::ssh_key::public::RsaPublicKey) -> PublicKey {
    PublicKey::new(russh::keys::ssh_key::public::KeyData::Rsa(rsa.clone()), "")
}
impl std::fmt::Debug for RsaSigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaSigningKey").finish_non_exhaustive()
    }
}

pub(super) struct RsaAuthSigner {
    key: Arc<RsaSigningKey>,
    identity: AgentIdentity,
    hash: HashAlg,
    used: bool,
}
impl RsaAuthSigner {
    pub(super) fn new(key: Arc<RsaSigningKey>, identity: AgentIdentity, hash: HashAlg) -> Self {
        Self {
            key,
            identity,
            hash,
            used: false,
        }
    }
}
impl std::fmt::Debug for RsaAuthSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RsaAuthSigner")
            .field("used", &self.used)
            .finish_non_exhaustive()
    }
}
impl russh::Signer for RsaAuthSigner {
    type Error = RsaKeyError;
    async fn auth_sign(
        &mut self,
        identity: &AgentIdentity,
        hash: Option<HashAlg>,
        mut data: Vec<u8>,
    ) -> Result<Vec<u8>, Self::Error> {
        let same = match (&self.identity, identity) {
            (AgentIdentity::PublicKey { key: a, .. }, AgentIdentity::PublicKey { key: b, .. }) => {
                a.key_data() == b.key_data()
            }
            (
                AgentIdentity::Certificate { certificate: a, .. },
                AgentIdentity::Certificate { certificate: b, .. },
            ) => a == b,
            _ => false,
        };
        if self.used || !same || hash != Some(self.hash) || data.len() > 128 * 1024 {
            return Err(RsaKeyError::Identity);
        }
        self.used = true;
        let permit = permit()?;
        let key = self.key.clone();
        let hash = self.hash;
        // Permit stays with a cancelled blocking operation until it finishes.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let signature = key.sign_checked(hash, &data)?;
            let algorithm = match hash {
                HashAlg::Sha256 => "rsa-sha2-256",
                HashAlg::Sha512 => "rsa-sha2-512",
                _ => return Err(RsaKeyError::Identity),
            };
            let size = 8 + algorithm.len() + signature.len();
            data.extend_from_slice(&(size as u32).to_be_bytes());
            data.extend_from_slice(&(algorithm.len() as u32).to_be_bytes());
            data.extend_from_slice(algorithm.as_bytes());
            data.extend_from_slice(&(signature.len() as u32).to_be_bytes());
            data.extend_from_slice(&signature);
            Ok(data)
        })
        .await
        .map_err(|_| RsaKeyError::Worker)?
    }
}
