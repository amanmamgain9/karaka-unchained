//! SPKI PEM public keys, read as jose's `importSPKI` reads them for one
//! algorithm: the key's type must be the algorithm's.

use ring::signature::{self, UnparsedPublicKey, VerificationAlgorithm};

use crate::base64::decode_standard;

/// A signature algorithm a key is configured for. (Karaka:
/// `algorithm`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Es256,
    Rs256,
    EdDsa,
}

impl Algorithm {
    /// The JWS `alg` name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Es256 => "ES256",
            Self::Rs256 => "RS256",
            Self::EdDsa => "EdDSA",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "ES256" => Some(Self::Es256),
            "RS256" => Some(Self::Rs256),
            "EdDSA" => Some(Self::EdDsa),
            _ => None,
        }
    }
}

/// An imported key: what to verify with.
pub(crate) struct PublicKey {
    pub algorithm: Algorithm,
    key: UnparsedPublicKey<Vec<u8>>,
}

impl PublicKey {
    pub fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        self.key.verify(message, signature).is_ok()
    }
}

const SPKI_INVALID: &str = "\"spki\" must be SPKI formatted string";

/// One DER element: its tag, and its content.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7f);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let length = rest[..count]
            .iter()
            .fold(0_usize, |length, byte| (length << 8) | usize::from(*byte));
        (length, &rest[count..])
    };
    if rest.len() < length {
        return None;
    }
    Some((tag, &rest[..length], &rest[length..]))
}

const EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];
const P256: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07];
const RSA_ENCRYPTION: &[u8] = &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01];
const ED25519: &[u8] = &[0x2b, 0x65, 0x70];

/// The PEM's DER: one `PUBLIC KEY` block.
fn pem_body(pem: &str) -> Option<Vec<u8>> {
    let pem = pem.trim();
    let body = pem
        .strip_prefix("-----BEGIN PUBLIC KEY-----")?
        .strip_suffix("-----END PUBLIC KEY-----")?;
    decode_standard(body)
}

/// Imports `pem` for `algorithm`. (jose: `importSPKI`.)
///
/// # Errors
///
/// It isn't an SPKI PEM, or not one of `algorithm`'s key type, or an RSA
/// key shorter than 2048 bits, in jose's words.
pub(crate) fn import(pem: &str, algorithm: Algorithm) -> Result<PublicKey, String> {
    let der = pem_body(pem).ok_or(SPKI_INVALID)?;
    let (tag, spki, rest) = element(&der).ok_or(SPKI_INVALID)?;
    if tag != 0x30 || !rest.is_empty() {
        return Err(SPKI_INVALID.into());
    }
    let (tag, identifier, rest) = element(spki).ok_or(SPKI_INVALID)?;
    let (bits_tag, bits, trailing) = element(rest).ok_or(SPKI_INVALID)?;
    if tag != 0x30 || bits_tag != 0x03 || !trailing.is_empty() {
        return Err(SPKI_INVALID.into());
    }
    let (&unused, key) = bits.split_first().ok_or(SPKI_INVALID)?;
    if unused != 0 {
        return Err(SPKI_INVALID.into());
    }
    let (oid_tag, oid, parameters) = element(identifier).ok_or(SPKI_INVALID)?;
    if oid_tag != 0x06 {
        return Err(SPKI_INVALID.into());
    }
    let mismatch = || "Invalid key type for the given algorithm".to_owned();
    let verifier: &'static dyn VerificationAlgorithm = match algorithm {
        Algorithm::Es256 => {
            let (curve_tag, curve, _) = element(parameters).ok_or_else(mismatch)?;
            if oid != EC_PUBLIC_KEY || curve_tag != 0x06 || curve != P256 {
                return Err(mismatch());
            }
            &signature::ECDSA_P256_SHA256_FIXED
        }
        Algorithm::Rs256 => {
            if oid != RSA_ENCRYPTION {
                return Err(mismatch());
            }
            if modulus_bits(key).ok_or(SPKI_INVALID)? < 2048 {
                return Err("RS256 requires key modulusLength to be 2048 bits or larger".into());
            }
            &signature::RSA_PKCS1_2048_8192_SHA256
        }
        Algorithm::EdDsa => {
            // Ed448, which jose also takes for EdDSA, has no ring verifier.
            if oid != ED25519 || key.len() != 32 {
                return Err(mismatch());
            }
            &signature::ED25519
        }
    };
    Ok(PublicKey {
        algorithm,
        key: UnparsedPublicKey::new(verifier, key.to_vec()),
    })
}

/// The bit length of an `RSAPublicKey`'s modulus.
fn modulus_bits(key: &[u8]) -> Option<usize> {
    let (tag, sequence, _) = element(key)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, modulus, _) = element(sequence)?;
    if tag != 0x02 {
        return None;
    }
    let modulus = match modulus.iter().position(|&byte| byte != 0) {
        Some(start) => &modulus[start..],
        None => return Some(0),
    };
    Some(modulus.len() * 8 - modulus[0].leading_zeros() as usize)
}
